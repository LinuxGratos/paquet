# Scripts et hooks

Deux mécanismes permettent d'agir sur le système au-delà de la copie des
fichiers :

| | Scripts | Hooks |
|---|---|---|
| Appartiennent à | un paquet (sa recette) | le système (`/etc/pq/hooks/`) |
| S'exécutent | autour de l'installation, de la mise à jour ou de la suppression *de ce paquet* | une fois à la fin d'une commande, si des fichiers installés ou supprimés correspondent à leurs chemins |
| Exemple | créer l'utilisateur d'un service | `ldconfig` après l'installation de bibliothèques |

Les deux s'exécutent avec `/bin/sh`, en root, dans le système géré (dans le
chroot avec `--chroot`), depuis `/`, sans entrée standard. `paquet
--noscripts` ne lance ni les uns ni les autres.

## Les scripts d'un paquet

### Les écrire

Dans le répertoire de la recette, à côté du `Pkgfile`, à la manière de CRUX :

```text
recettes/extra/serveur/
├── Pkgfile
├── pre-install
├── post-install
├── pre-remove
└── post-remove
```

Chaque fichier est facultatif. `pqmake` les range dans l'archive du paquet
principal (pas dans ses sous-paquets `.devel`, `.doc`...), sous les noms
`.PRE-INSTALL`, `.POST-INSTALL`, `.PRE-REMOVE` et `.POST-REMOVE`. Ils ne sont
jamais déballés dans le système.

Un exemple de `post-install` :

```sh
# $1 : install ou upgrade ; $2 : nouvelle version ; $3 : ancienne version
if [ "$1" = install ]; then
    getent group serveur >/dev/null || groupadd -r serveur
    getent passwd serveur >/dev/null || useradd -r -g serveur -d /var/lib/serveur serveur
fi
chown -R serveur:serveur /var/lib/serveur
```

### Quand s'exécutent-ils ?

| Script | Moment | Arguments | Si le script échoue |
|---|---|---|---|
| `pre-install` | avant de déballer les fichiers | `install <version>` ou `upgrade <version> <ancienne version>` | l'installation ou la mise à jour est annulée, le système reste tel quel |
| `post-install` | une fois le paquet installé ou mis à jour | idem | le paquet reste installé, l'erreur est signalée et la commande se termine en échec |
| `pre-remove` | avant de supprimer les fichiers | `remove <version>` | la suppression est annulée |
| `post-remove` | une fois les fichiers supprimés | `remove <version>` | le paquet reste supprimé, l'erreur est signalée |

Les versions sont de la forme `<version>-<release>` (`8.2-1`). La variable
`PQ_PACKAGE` contient le nom du paquet.

Lors d'une mise à jour, ce sont les scripts `pre-install` et `post-install`
de la *nouvelle* version qui s'exécutent (avec `upgrade`) ; les scripts de
suppression de l'ancienne version ne sont pas lancés.

Les scripts d'un paquet installé sont gardés dans
`/var/lib/pq/scripts/<paquet>/` : c'est de là que viennent `pre-remove` et
`post-remove` au moment de la suppression. `paquet -I <paquet>` les liste.

### Un script qui échoue avant l'installation

`pre-install` s'exécute à l'intérieur de la transaction : s'il échoue, rien
n'a encore été déballé, et le système revient exactement à son état
précédent. Mais `paquet` ne peut pas défaire ce que le script lui-même a
modifié avant d'échouer : un script `pre-install` doit donc vérifier avant
d'agir, ou ne faire que des actions sans conséquence si elles sont refaites.

### Système en cours d'installation

Quand le système géré n'a pas encore de `/bin/sh` (par exemple pendant
l'installation des tout premiers paquets dans une racine vide avec
`--chroot`), les scripts sont sautés, avec un avertissement :

```text
Warning: no /bin/sh in /mnt/great-os, the post-install script of base-files is skipped
```

## Les hooks

### Le principe

Certaines actions concernent tout le système plutôt qu'un paquet : refaire
le cache des bibliothèques après l'installation d'une `.so`, celui des
icônes après celle d'un thème... Les écrire dans le `post-install` de chaque
paquet concerné serait répétitif, et lancerait l'action une fois par paquet.

Un hook décrit l'action une seule fois, avec les chemins qui la déclenchent.
À la fin de chaque commande (`-i`, `-u`, `-r`, `autoremove`), `paquet`
regarde les fichiers installés et supprimés par toute la commande, et lance
une fois chaque hook concerné. Installer dix bibliothèques avec leurs
dépendances lance donc `ldconfig` une seule fois.

### Les écrire

Un hook est un fichier `/etc/pq/hooks/<nom>.hook`, au format `CLÉ=valeur` :

```sh
Description=Updating the shared library cache
Path=lib/*.so*
Path=usr/lib/*.so*
On=install remove
Exec=/sbin/ldconfig
```

| Clé | Rôle |
|---|---|
| `Description` | texte affiché quand le hook s'exécute (facultatif) |
| `Path` | motif de chemin, relatif à `/` ; répétable. `*` remplace n'importe quels caractères, `/` compris ; `?` un seul caractère |
| `On` | `install`, `remove` ou `install remove` (par défaut : les deux) |
| `Exec` | commande lancée avec `/bin/sh -c` ; une valeur entièrement entourée de guillemets perd ses guillemets |

Les hooks s'exécutent dans l'ordre alphabétique de leurs fichiers. Un hook
qui échoue est signalé et la commande se termine en échec ; les paquets
restent installés ou supprimés.

### Qui les fournit ?

Un hook est livré par le paquet qui fournit l'outil qu'il appelle : le
paquet de la glibc installe `ldconfig.hook`, celui de GTK installe le hook du
cache d'icônes, etc. Ainsi, un système sans GTK n'a pas de hook GTK, et le
hook arrive en même temps que l'outil. Comme tout fichier de `/etc`, un hook
modifié par l'administrateur n'est jamais écrasé par une mise à jour.

Le dépôt de `paquet` contient des exemples prêts à empaqueter, dans
[`hooks/`](../hooks/) :

| Fichier | Paquet qui le fournit | Action |
|---|---|---|
| `ldconfig.hook` | glibc | `ldconfig` quand des bibliothèques changent |
| `gtk-icon-cache.hook` | gtk | cache des thèmes d'icônes modifiés |
| `glib-schemas.hook` | glib | compilation des schémas GSettings |
| `desktop-database.hook` | desktop-file-utils | base des fichiers `.desktop` |

Pour en livrer un dans un paquet, la recette l'installe simplement :

```bash
install -Dm644 $PKGMK_ROOT/ldconfig.hook $PKG/etc/pq/hooks/ldconfig.hook
```
