# Générer et publier un dépôt

Un dépôt (ou miroir) Great-OS est un simple arbre de fichiers, servi en HTTP(S)
ou lu en local (`file://`). `genpqrepo` en génère l'index, `PQREPO`, que
`paquet --sync` récupère.

## Organisation du miroir

```text
/srv/great-os/                         <- MIRROR="https://miroir.exemple.org/great-os"
├── stable/                            <- BRANCH="stable"
│   ├── x86_64/                        <- architecture (uname -m)
│   │   ├── PQREPO                     <- index
│   │   ├── PQREPO.sig                 <- signature de l'index
│   │   ├── base/                      <- dossier (l'ensemble de la recette)
│   │   │   ├── libsalut-1.2-1.tar.zst
│   │   │   ├── libsalut.devel-1.2-1.tar.zst
│   │   │   └── nano-8.2-1.tar.zst
│   │   └── extra/
│   │       └── salut-0.3-2.tar.zst
│   └── aarch64/
│       └── ...
└── testing/
    └── x86_64/
        └── ...
```

Chaque couple branche/architecture (`stable/x86_64`, `testing/x86_64`...) est
un dépôt indépendant, avec son propre index. Les clients choisissent leur
branche avec `BRANCH` dans `/etc/pq.conf` ; l'architecture est celle de la
machine.

`pqmake` dépose directement ses archives dans cette organisation : son
`REPO_DIR` est le répertoire d'une branche et d'une architecture, par
exemple `/srv/great-os/stable/x86_64`.

## Les archives

Une archive s'appelle `<nom>-<version>-<release>.tar.zst` : une archive tar
compressée avec zstd, qui contient les fichiers du paquet relativement à `/`,
un fichier `.META` à la racine et, facultativement, les scripts du paquet
(`.PRE-INSTALL`, `.POST-INSTALL`, `.PRE-REMOVE`, `.POST-REMOVE`, voir
[Scripts et hooks](scripts.md)). Ces fichiers à la racine ne sont jamais
déballés dans le système. Le `.META` ressemble à :

```text
Description: Dit bonjour
Groups:
Set: extra
Depends: libsalut
```

| Ligne | Contenu |
|---|---|
| `Description:` | description du paquet |
| `Groups:` | groupe du sous-paquet (`devel`, `doc`...), vide pour un paquet principal |
| `Set:` | ensemble |
| `Depends:` | dépendances à l'exécution, séparées par des espaces ou des virgules ; facultatif, et peut être répété pour une longue liste |

`pqmake` produit ces archives. Pour en fabriquer une à la main :

```sh
mkdir -p racine/usr/bin && cp outil racine/usr/bin/
printf 'Description: Un outil\nGroups:\nSet: extra\n' > racine/.META
tar --zstd --sort=name --owner=0 --group=0 --numeric-owner \
    -cf /srv/great-os/stable/x86_64/extra/outil-1.0-1.tar.zst -C racine .
```

## Générer l'index

```sh
genpqrepo -k ~/cles/great-os.key /srv/great-os/stable/x86_64
```

```text
PQREPO file generated successfully at /srv/great-os/stable/x86_64/PQREPO
PQREPO signed: /srv/great-os/stable/x86_64/PQREPO.sig
```

`genpqrepo` parcourt toutes les archives `*.tar.zst` du répertoire et de ses
sous-dossiers, lit leur `.META`, calcule leur somme SHA-256 et écrit une ligne
par paquet dans `PQREPO` :

```text
052e1423...bcb93;1791869940;extra;salut;0.3;2;Dit bonjour;;extra;;libsalut
```

| Champ | Contenu |
|---|---|
| 1 | somme SHA-256 de l'archive |
| 2 | date de modification de l'archive (secondes depuis 1970) |
| 3 | dossier de l'archive dans le dépôt |
| 4, 5, 6 | nom, version, release |
| 7, 8, 9 | description, groupe, ensemble (lus dans `.META`) |
| 10 | état : vide dans l'index, rempli par `paquet` sur les clients |
| 11 | dépendances, séparées par des espaces |

La somme SHA-256 est ce qui protège les archives : `paquet` refuse une
archive dont la somme ne correspond pas à celle de l'index. Une fois l'index
signé, modifier une archive sans régénérer et resigner l'index la rend donc
inutilisable (voir [Signer un dépôt](signature.md)).

L'index doit être régénéré après chaque ajout, remplacement ou suppression
d'archive. `pqmake --index` le fait à la fin de ses constructions.

### Plusieurs versions d'un même paquet

Si le dépôt contient plusieurs versions d'un paquet, seule la plus récente
(dans l'ordre de `sort -V` sur `<version>-<release>`) est indexée, et les
autres sont signalées :

```text
Warning: salut 0.3-2 ignored, salut 0.3-3 is newer
```

Les anciennes archives ne servent alors plus à rien pour les clients à jour ;
on peut les supprimer, puis régénérer l'index. Mieux vaut les garder jusqu'à
ce que les clients aient fait un `--sync` : un client qui a encore l'ancien
index cherche encore l'ancienne archive.

### Ne régénérer qu'un dossier

```sh
genpqrepo -k ~/cles/great-os.key /srv/great-os/stable/x86_64 extra
```

Ne relit que les archives de `extra/` (et de ses sous-dossiers) et garde
telles quelles les lignes des autres dossiers : utile pour un gros dépôt dont
un seul dossier a changé.

### Archives refusées

Une archive dont le nom ne suit pas `<nom>-<version>-<release>.tar.zst`, ou
utilise d'autres caractères que lettres, chiffres et `. _ + -` (nom) ou
`. _ + ~` (version, release), est ignorée avec un avertissement. Une archive
sans `.META` est indexée avec une description vide, et signalée.

### Référence

```text
genpqrepo [-k <clé_privée>] <répertoire> [dossier]
genpqrepo --genkey <nom>
```

| Variable | Rôle |
|---|---|
| `PQ_SIGN_KEY` | clé privée de signature, si `-k` n'est pas donné |
| `OUTPUT_FILE` | chemin de l'index (défaut : `<répertoire>/PQREPO`, signature dans `<OUTPUT_FILE>.sig`) |

## Publier

Le miroir est un arbre de fichiers statiques : n'importe quel serveur web
convient (nginx, Apache, un hébergement de fichiers...). Seuls `GET` et les
codes HTTP standard sont utilisés.

Pour mettre à jour un miroir distant, envoyez d'abord les archives, puis
l'index et sa signature ensemble, en dernier :

```sh
D=/srv/great-os/stable/x86_64
rsync -a --exclude PQREPO --exclude PQREPO.sig $D/ miroir:/var/www/great-os/stable/x86_64/
rsync -a $D/PQREPO $D/PQREPO.sig miroir:/var/www/great-os/stable/x86_64/
```

Ainsi, un client ne voit jamais un index qui annonce une archive pas encore
arrivée. Un client qui récupère l'index et la signature pendant leur
remplacement obtient un couple qui ne correspond pas : `--sync` le refuse,
garde l'index précédent, et il suffit de recommencer.

Les clés privées n'ont rien à faire sur le miroir : seuls `PQREPO`,
`PQREPO.sig` et les archives y sont publiés.

## Tester un dépôt localement

Un dépôt local se lit avec une URL `file://`, sans serveur. Pour l'essayer
sur un système jetable, sans toucher au système courant :

```sh
mkdir -p /tmp/essai/keys /tmp/essai/racine
cp ~/cles/great-os.pub /tmp/essai/keys/
echo 'MIRROR="file:///srv/great-os"' > /tmp/essai/pq.conf

export PQ_CONFIG=/tmp/essai/pq.conf PQ_KEYS_DIR=/tmp/essai/keys
paquet --sync --chroot /tmp/essai/racine
paquet -i salut --chroot /tmp/essai/racine
ls -R /tmp/essai/racine
```

`--chroot` installe sous `/tmp/essai/racine`, avec sa propre base de paquets,
et `PQ_CONFIG`/`PQ_KEYS_DIR` remplacent `/etc/pq.conf` et `/etc/pq/keys`.
Pour un dépôt de test non signé, ajoutez `--nosign` à `--sync`.
