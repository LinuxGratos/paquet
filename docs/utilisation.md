# Utilisation de paquet

`paquet` installe, met à jour et supprime les paquets d'un système Great-OS.
`pq` est le même programme sous un nom plus court : `pq -i nano` équivaut à
`paquet -i nano`.

Les commandes qui modifient le système (`--sync`, `-i`, `-r`, `-u`,
`autoremove`, `--mark-*`, `-C`, `--purge-old-cache`, `--recover`) doivent être
lancées en root. Les autres (`-l`, `-s`, `-o`, `-Q`, `-I`, `-F`,
`--cache-info`) peuvent l'être par n'importe quel utilisateur.

## Configuration

### Le miroir : `/etc/pq.conf`

```sh
MIRROR="https://miroir.exemple.org/great-os"
BRANCH="stable"
PARALLEL_DOWNLOADS=4
GROUP="fr"
```

| Réglage | Rôle | Par défaut |
|---|---|---|
| `MIRROR` | adresse du miroir : `https://`, `http://` ou `file://` | (obligatoire) |
| `BRANCH` | branche suivie (`stable`, `testing`...) | `stable` |
| `PARALLEL_DOWNLOADS` | nombre de téléchargements simultanés (1 à 16) | `4` |
| `GROUP` | langue ou groupe installé avec chaque paquet ; répétable (voir [Langues et sous-paquets](#langues-et-sous-paquets)) | aucun : l'anglais seul |

Les paquets sont cherchés dans `$MIRROR/$BRANCH/<architecture>/`, où
l'architecture est celle de `uname -m` (`x86_64`, `aarch64`...). Avec la
configuration ci-dessus, sur un PC 64 bits, l'index est
`https://miroir.exemple.org/great-os/stable/x86_64/PQREPO`.

Le fichier est lu, jamais exécuté : seules les lignes `CLÉ=valeur` (avec
d'éventuels guillemets et un `export` devant) sont prises en compte, les
autres sont signalées et ignorées.

Les variables de proxy habituelles (`https_proxy`, `http_proxy`,
`no_proxy`...) sont respectées, et les certificats HTTPS sont vérifiés avec
ceux du système (`/etc/ssl/certs`).

### Les clés de confiance : `/etc/pq/keys/`

`paquet --sync` refuse un index qui n'est pas signé par l'une des clés
publiques `/etc/pq/keys/*.pub`. Pour faire confiance au dépôt officiel :

```sh
install -Dm644 great-os.pub /etc/pq/keys/great-os.pub
```

La clé publique est fournie par le mainteneur du dépôt (voir
[Signer un dépôt](signature.md)). Vérifiez-la par un autre canal que le
miroir lui-même (site web, système installé, contact direct) : c'est elle
qui garantit tout le reste.

## Au quotidien

### Mettre à jour la liste des paquets

```console
# paquet --sync
Fetching repository file from https://miroir.exemple.org/great-os/stable/x86_64/PQREPO
Signature verified with great-os.pub
Repository file fetched successfully!
Merging with local state...
Merge completed!
```

À faire avant d'installer ou de mettre à jour. Si la signature est absente
ou invalide, l'index est refusé et l'index précédent reste en place.

### Chercher

```console
$ paquet -s salut
Searching for packages matching: salut
Name: salut
Version: 0.3-2
Description: Dit bonjour
Folder: extra
Groups:
Set: extra
Depends: libsalut
Status: N
```

La recherche porte sur le nom, la description, le dossier et l'ensemble, sans
tenir compte des majuscules. `paquet -l` liste tous les paquets connus, une
ligne par paquet.

### Installer

```console
# paquet -i salut
Installing salut and its dependencies: libsalut salut
Downloading 2 packages (1.3 MiB):
  ✓ salut 0.3-2       96.4 KiB
  ✓ libsalut 1.2-1     1.2 MiB
  1.3 MiB downloaded in 0.8 s, 1.6 MiB/s
Installing package: libsalut
Extracting package files...
libsalut installed successfully!
Installing package: salut
Extracting package files...
salut installed successfully!
```

Les dépendances manquantes sont installées d'abord. Toutes les archives sont
téléchargées avant que le système ne soit touché (voir
[Les téléchargements](#les-téléchargements)), et chacune est vérifiée (somme
SHA-256 de l'index signé).

L'installation est refusée si elle écraserait un fichier qui existe déjà
(fichier d'un autre paquet, ou créé à la main) ; la liste des fichiers en
cause est affichée et rien n'est modifié. Seule exception, les fichiers de
`/etc` : voir [Les fichiers de configuration](#les-fichiers-de-configuration).

### Mettre à jour

```console
# paquet --sync
# paquet -u
Packages available for upgrade (in upgrade order):
  libsalut  1.2-1 -> 1.3-1
  salut     0.3-2 -> 0.4-1
New dependencies to install: libbonjour
Download size: 1.6 MiB (1 archive already in the cache)
Do you want to upgrade these packages? [Y/n]
```

`paquet -u` met à jour tous les paquets marqués `U` (une nouvelle version est
dans l'index), les dépendances d'abord, après confirmation. Avant la
question, il affiche les versions, les nouvelles dépendances et la taille
totale à télécharger. `paquet -u -y` ne pose pas la question (pour un script
ou une tâche planifiée).

`paquet -u nano` ne met à jour que `nano`, sans confirmation. Les nouvelles
dépendances d'une nouvelle version sont installées automatiquement ; les
fichiers que la nouvelle version ne fournit plus sont supprimés.

### Les téléchargements

`paquet -i` et `paquet -u` téléchargent d'abord toutes les archives
nécessaires, plusieurs à la fois (`PARALLEL_DOWNLOADS`, 4 par défaut), puis
seulement installent. Une archive manquante sur le miroir, ou dont la somme
ne correspond pas à l'index, arrête donc la commande avant toute
modification du système :

```console
# paquet -u -y
...
Downloading 2 packages (1.2 MiB + 1 of unknown size):
  ✓ libsalut 1.3-1     1.2 MiB
  ✗ salut 0.4-1         failed
Error: failed to download https://miroir.exemple.org/great-os/stable/x86_64/extra/salut-0.4-1.tar.zst: http status: 404
Error: 1 download of 2 failed; nothing was changed.
```

Les archives bien arrivées restent dans le cache : la commande suivante ne
télécharge que ce qui manque. Un transfert interrompu par le réseau est
retenté une fois.

Dans un terminal, une ligne de progression sous la liste des archives
terminées donne l'avancement, la quantité reçue et le débit ; elle disparaît
à la fin :

```text
Downloading 5 packages (30.5 MiB):
  ✓ beta 1.0-1        5.9 MiB
  ✓ epsilon 1.0-1     6.5 MiB
  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━───────────   72%   22.0 MiB / 30.5 MiB   19.9 MiB/s
```

Hors terminal (redirection vers un fichier, tâche planifiée), seules les
lignes des archives terminées sont écrites. Sans locale UTF-8, la barre est
faite de `#` et de `-`, sans les symboles ✓ et ✗ ; la variable `NO_COLOR`
supprime les couleurs.

La taille annoncée vient du miroir (requête HTTP `HEAD`) : si le serveur ne
la donne pas, elle est signalée comme inconnue (`+ 1 of unknown size`) et la
barre n'affiche que la quantité reçue.

### Supprimer

```console
# paquet -r salut
Removing package: salut
Keeping protected file: /etc/salut.conf
Removing file: /usr/bin/salut
salut removed successfully!
1 orphan package(s) left: libsalut. Run 'paquet autoremove' to remove them.
```

Un paquet dont dépend un autre paquet installé ne peut pas être supprimé :
`paquet` liste les paquets qui en ont besoin. Les fichiers de `/etc` et de
`/var/lib/pq` sont toujours conservés ; les répertoires ne sont supprimés que
s'ils sont vides.

### Langues et sous-paquets

Les traductions d'un paquet sont dans des sous-paquets, un par langue :
`nano.fr`, `nano.de`... Le paquet `nano` lui-même ne contient que l'anglais.
Pour avoir ses langues, on les déclare dans `/etc/pq.conf`, une ligne par
langue (ou plusieurs langues sur une ligne) :

```sh
GROUP="fr"
GROUP="it"
```

Les pages de manuel anglaises sont dans le paquet principal, les traduites
dans le paquet de leur langue. `GROUP` accepte aussi les autres groupes de
sous-paquets : `GROUP="doc"` installe la documentation (`nano.doc`) avec
chaque paquet, `GROUP="devel"` les fichiers de développement...

Avec `GROUP="fr"` :

- `paquet -i nano` installe aussi `nano.fr`, et le paquet français de
  chaque dépendance installée (`ncurses.fr`...), quand ils existent ;
- `paquet -u` installe ceux qui manquent : après l'ajout d'une ligne
  `GROUP`, ou quand une nouvelle version apporte une traduction ;

  ```text
  Packages to add for GROUP fr: nano.fr ncurses.fr
  Download size: 412.0 KiB
  Do you want to upgrade these packages? [Y/n]
  ```
- `paquet -r nano` retire aussi `nano.fr` ;
- ces sous-paquets sont installés automatiquement (état `A`) et ne sont
  jamais orphelins tant que leur paquet est là. Après le retrait d'une ligne
  `GROUP`, ils le deviennent : `paquet autoremove` les supprime.

Sans aucune ligne `GROUP`, seul l'anglais est installé. Un sous-paquet peut
toujours être installé à la main : `paquet -i nano.de`.

### Les dépendances devenues inutiles

Un paquet installé comme dépendance (état `A`) devient orphelin quand plus
aucun paquet installé explicitement (état `I`) n'en a besoin, directement ou
non.

```sh
paquet -o              # liste les orphelins
paquet autoremove      # les supprime, après confirmation (-y : sans)
```

Pour garder un paquet installé comme dépendance, ou au contraire le laisser
partir avec le reste :

```sh
paquet --mark-explicit libsalut    # A -> I : ne sera plus jamais orphelin
paquet --mark-auto libsalut        # I -> A
```

`paquet -i` sur un paquet déjà installé comme dépendance le marque aussi comme
installé explicitement.

### Consulter

`paquet -I` affiche ce que l'on sait d'un paquet, installé ou non :

```console
$ paquet -I salut
Name           : salut
Version        : 0.3-3
Description    : Dit bonjour
Set            : extra
Folder         : extra
Groups         : -
Depends        : libsalut
Status         : I (installed explicitly)
Required by    : -
Files          : 2
Installed size : 47 B
Scripts        : -
```

Pour un paquet installé s'ajoutent les paquets installés qui en dépendent
(`Required by`), le nombre de fichiers, la place occupée et ses scripts
(voir [Scripts et hooks](scripts.md)). Si une mise à jour est disponible, la
ligne `Available` donne la nouvelle version.

`paquet -Q` dit à quel paquet appartient un fichier, `paquet -F` liste les
fichiers d'un paquet installé :

```console
$ paquet -Q /usr/lib/libsalut.so.1
/usr/lib/libsalut.so.1 is owned by libsalut 1.2-1
$ paquet -F salut
/etc
/etc/salut.conf
/usr/bin
/usr/bin/salut
```

Le chemin donné à `-Q` peut être relatif (au répertoire courant) ou passer
par un lien symbolique (`/lib/libz.so.1` quand `/lib` pointe vers
`usr/lib`). Un fichier qui n'appartient à aucun paquet fait échouer la
commande. `-F` liste les répertoires que le paquet a créés en plus de ses
fichiers.

## Scripts et hooks

Un paquet peut fournir des scripts, lancés avant ou après son installation,
sa mise à jour ou sa suppression (créer l'utilisateur d'un service, par
exemple). Le système peut avoir des hooks, dans `/etc/pq/hooks/`, lancés une
fois à la fin d'une commande quand des fichiers qui les concernent ont été
installés ou supprimés (`ldconfig` après l'installation de bibliothèques,
par exemple) :

```text
Running the post-install script of serveur...
Running hook: Updating the shared library cache
```

Un script `pre-install` ou `pre-remove` qui échoue annule l'opération. Un
script `post-install` ou `post-remove`, ou un hook, qui échoue est signalé et
fait échouer la commande, mais l'opération reste faite. `--noscripts` (avec
`-i`, `-r`, `-u` et `autoremove`) ne lance ni scripts ni hooks : à réserver au
dépannage, puisque le paquet peut alors rester incomplet (un utilisateur
non créé, un cache non mis à jour).

Le détail est dans [Scripts et hooks](scripts.md).

## Les états

`paquet -l` et `paquet -s` affichent l'état de chaque paquet :

| État | Sens |
|---|---|
| `N` | non installé |
| `I` | installé explicitement |
| `A` | installé automatiquement, comme dépendance |
| `…U` | une mise à jour est disponible (`IU`, `AU`) |
| `…X` | n'est plus fourni par le dépôt (`IX`, `AX`) : il reste installé, mais ne sera plus mis à jour |

## Les fichiers de configuration

Un fichier de `/etc` qui existe déjà n'est jamais écrasé, ni à
l'installation, ni à la mise à jour. Si le paquet en fournit une version
différente, elle est posée à côté, avec l'extension `.pqnew` :

```text
Keeping existing /etc/salut.conf, new version saved as /etc/salut.conf.pqnew
```

C'est à l'administrateur de comparer les deux (`diff /etc/salut.conf
/etc/salut.conf.pqnew`), de reporter ce qui l'intéresse, puis de supprimer le
`.pqnew`. `paquet` ne supprime pas les `.pqnew`, même à la suppression du
paquet.

## Le cache

Les archives téléchargées sont gardées dans `/var/lib/pq/cache/`. Une archive
déjà dans le cache, avec la bonne somme, n'est pas retéléchargée.

```sh
paquet --cache-info        # contenu et taille du cache
paquet --purge-old-cache   # supprime les anciennes versions
paquet -C                  # affiche la taille et propose de tout vider
```

## Pannes et interruptions

Une installation ou une mise à jour se fait entièrement ou pas du tout :

- après une erreur (disque plein, archive abîmée...) ou un Ctrl-C, le système
  est aussitôt remis dans son état précédent ;
- après un plantage (coupure de courant, `kill -9`), la prochaine commande qui
  modifie le système annule d'abord l'opération inachevée. `paquet --recover`
  le fait explicitement ; en attendant, les autres commandes préviennent :

```text
Warning: an interrupted operation is pending; run 'paquet --recover' as root.
```

Une suppression interrompue n'est pas annulée, mais peut simplement être
relancée.

Une seule instance de `paquet` peut modifier un système à la fois ; une
seconde s'arrête aussitôt :

```text
Error: another instance of paquet is running (lock: /var/lib/pq/lock).
```

## Gérer un autre système : `--chroot`

`--chroot <racine>` applique la commande au système installé sous
`<racine>` (une partition montée, une image en préparation...) :

```sh
paquet --sync --chroot /mnt/great-os
paquet -i base --chroot /mnt/great-os
```

Les fichiers sont installés sous la racine, et toute la base de paquets est
prise sous `<racine>/var/lib/pq/`. La configuration (`/etc/pq.conf`) et les
clés (`/etc/pq/keys/`) restent celles du système qui lance la commande. Les
scripts des paquets et les hooks (ceux de `<racine>/etc/pq/hooks/`)
s'exécutent dans la racine, avec `chroot` : il faut donc être root, et la
racine doit avoir un `/bin/sh` (sinon ils sont sautés, avec un
avertissement).

## Référence

```text
paquet [OPTION] [--chroot <racine>]

  -S, --sync [--nosign]          récupère l'index, vérifie sa signature et met à jour l'état
  -i, --install <paquet>         installe un paquet et ses dépendances manquantes
  -l, --list                     liste les paquets
  -s, --search <terme>           cherche par nom, description, dossier ou ensemble
  -r, --remove <paquet>          supprime un paquet dont aucun paquet installé ne dépend
  -u, --upgrade [paquet]         met à jour un paquet, ou tous ceux marqués U
  -o, --orphans                  liste les dépendances devenues inutiles
  -Q, --query <fichier>          indique le paquet auquel appartient un fichier
  -I, --info <paquet>            affiche ce que l'on sait d'un paquet
  -F, --files <paquet>           liste les fichiers d'un paquet installé
  autoremove, --autoremove       les supprime
  --mark-auto <paquet>           marque un paquet comme installé automatiquement
  --mark-explicit <paquet>       marque un paquet comme installé explicitement
  --cache-info                   affiche le cache et sa taille
  --purge-old-cache              supprime du cache les anciennes versions
  -C, --cache                    affiche la taille du cache et propose de le vider
  --recover                      annule une opération interrompue par un plantage
  --chroot <racine>              gère le système installé sous <racine>
  --noscripts                    ne lance ni scripts ni hooks (avec -i, -r, -u, autoremove)
  -y, --yes                      ne demande pas de confirmation (-u, autoremove, -C)
  -h, --help                     affiche l'aide
```

`--nosign` désactive la vérification de signature de `--sync`. Il n'est utile
que pour un dépôt local de test : avec lui, n'importe qui capable de modifier
le miroir ou le trafic réseau peut faire installer ce qu'il veut.

| Fichier | Contenu |
|---|---|
| `/etc/pq.conf` | `MIRROR`, `BRANCH`, `PARALLEL_DOWNLOADS` et `GROUP` (variable `PQ_CONFIG` pour un autre chemin) |
| `/etc/pq/keys/*.pub` | clés publiques de confiance (variable `PQ_KEYS_DIR`) |
| `/var/lib/pq/PQREPO` | index du dépôt, tel que récupéré |
| `/var/lib/pq/PQREPOSTATE` | état de chaque paquet connu |
| `/var/lib/pq/files/<paquet>` | fichiers installés par chaque paquet |
| `/var/lib/pq/cache/` | archives téléchargées |
| `/var/lib/pq/transaction/` | journaux des opérations en cours |
| `/var/lib/pq/scripts/<paquet>/` | scripts des paquets installés |
| `/etc/pq/hooks/*.hook` | hooks du système |
| `/var/lib/pq/lock` | verrou |

Codes de sortie : `0` succès, `1` erreur, `130` ou `143` interruption
(Ctrl-C, `kill`).
