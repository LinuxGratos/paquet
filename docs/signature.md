# Signer un dépôt

L'index d'un dépôt (`PQREPO`) est signé avec une clé Ed25519. Les clients ne
l'acceptent que si sa signature (`PQREPO.sig`) est vérifiée par l'une des
clés publiques qu'ils ont installées.

## Ce que la signature protège

L'index contient la somme SHA-256 de chaque archive, et `paquet` refuse une
archive dont la somme ne correspond pas. Signer l'index suffit donc à
authentifier tout le dépôt : un miroir compromis, un serveur intermédiaire ou
quelqu'un qui modifie le trafic réseau ne peut ni modifier un paquet, ni en
ajouter un, ni changer ses dépendances, sans que `paquet` le refuse.

Ce qu'elle ne protège pas : la fraîcheur. L'index n'a pas de date
d'expiration ; un miroir malveillant peut continuer à servir un ancien index
signé, et empêcher ainsi les clients de voir les mises à jour (sans pouvoir
leur faire installer autre chose que des paquets réellement publiés).

## Créer une clé

```console
$ genpqrepo --genkey ~/cles/great-os
Private key: /home/moi/cles/great-os.key (keep it secret, use it with -k or PQ_SIGN_KEY)
Public key:  /home/moi/cles/great-os.pub (install it in /etc/pq/keys on the clients)
```

| Fichier | Rôle | Où le mettre |
|---|---|---|
| `great-os.key` | clé privée (lisible par son seul propriétaire) : signe l'index | sur la machine qui génère l'index, et dans une sauvegarde hors ligne |
| `great-os.pub` | clé publique : vérifie les signatures | sur chaque client, dans `/etc/pq/keys/` |

`genpqrepo` refuse d'écraser une clé existante. Quiconque détient la clé
privée peut faire installer n'importe quoi aux clients : elle ne doit jamais
être publiée sur le miroir, ni versionnée avec les recettes.

Les clés sont au format PEM d'OpenSSL ; une clé créée avec OpenSSL convient
aussi :

```sh
openssl genpkey -algorithm ed25519 -out great-os.key
openssl pkey -in great-os.key -pubout -out great-os.pub
```

## Signer l'index

Avec `-k` :

```sh
genpqrepo -k ~/cles/great-os.key /srv/great-os/stable/x86_64
```

ou avec la variable `PQ_SIGN_KEY`, que `genpqrepo` et `pqmake --index`
utilisent toutes deux :

```sh
export PQ_SIGN_KEY=~/cles/great-os.key
pqmake --index -a
```

L'index est signé avant d'être écrit : si la signature échoue (clé illisible,
mauvais format), l'index et la signature précédents restent en place. Sans
clé, l'index est écrit sans signature, toute ancienne `PQREPO.sig` est
supprimée, et `genpqrepo` prévient que les clients le refuseront :

```text
Warning: the index is not signed (no -k or PQ_SIGN_KEY); paquet --sync will refuse it unless run with --nosign
```

Il faut resigner après chaque régénération de l'index : une signature ne vaut
que pour l'index exact qu'elle a signé.

La signature peut être vérifiée sans `paquet`, avec OpenSSL :

```console
$ openssl pkeyutl -verify -pubin -inkey great-os.pub -rawin -in PQREPO -sigfile PQREPO.sig
Signature Verified Successfully
```

## Installer la clé sur les clients

```sh
install -Dm644 great-os.pub /etc/pq/keys/great-os.pub
```

Tous les fichiers `*.pub` de `/etc/pq/keys/` sont acceptés : un index est
valide dès que l'une de ces clés vérifie sa signature. `--sync` indique
laquelle :

```text
Signature verified with great-os.pub
```

La première installation de la clé est le point de confiance de tout le
reste : elle doit venir d'une source sûre (l'image d'installation, le site du
projet en HTTPS, une remise en main propre), pas du miroir lui-même.

Une fois une clé en place, un paquet signé peut en ajouter d'autres (par
exemple un paquet `great-os-keys` qui installe
`/etc/pq/keys/great-os-2027.pub`). Mais `/etc` est protégé : `paquet`
n'écrase jamais un fichier de clé existant (la nouvelle version irait dans un
`.pqnew`, ignoré) et n'en supprime jamais, même en supprimant ou en mettant à
jour le paquet qui l'a installé. Une nouvelle clé doit donc porter un nouveau
nom de fichier, et retirer une clé se fait à la main :

```sh
rm /etc/pq/keys/great-os.pub
```

## Ce que voit un client

| Situation | Résultat de `paquet --sync` |
|---|---|
| index signé par une clé de confiance | accepté |
| index modifié après signature | `invalid signature (no key in /etc/pq/keys matches).` |
| signé par une clé inconnue | même message |
| pas de `PQREPO.sig` sur le miroir | `no signature found at .../PQREPO.sig` |
| aucune clé dans `/etc/pq/keys` | `no trusted public key found in /etc/pq/keys.` |

Dans tous les cas de refus, l'index local précédent est conservé :
`repository file rejected, the local index is unchanged.`

`paquet --sync --nosign` saute la vérification. À réserver à un dépôt local de
test : avec cette option, rien ne garantit l'origine des paquets installés.

## Changer de clé

Les clients acceptant plusieurs clés à la fois, on peut changer de clé sans
interruption :

1. créer la nouvelle clé, sous un autre nom (`genpqrepo --genkey
   great-os-2027`) ;
2. livrer la nouvelle clé publique aux clients, à côté de l'ancienne : par
   exemple une nouvelle version du paquet de clés, qui installe
   `/etc/pq/keys/great-os-2027.pub` et dont l'index est encore signé avec
   l'ancienne clé ;
3. une fois les clients à jour, signer l'index avec la nouvelle clé ;
4. retirer l'ancienne clé publique des clients (`rm
   /etc/pq/keys/great-os.pub` : `paquet` ne supprime pas les fichiers de
   `/etc`), puis détruire l'ancienne clé privée.

Tant que l'ancienne clé publique reste installée sur un client, un index
signé avec l'ancienne clé privée y est toujours accepté.

## Si la clé privée est compromise

Celui qui la détient peut signer un index qui fait installer ce qu'il veut. Il
faut :

1. créer une nouvelle clé ;
2. retirer l'ancienne clé publique de tous les clients (`rm`), et y
   installer la nouvelle, par un moyen qui ne dépend pas du dépôt (l'ancienne
   clé ne prouve plus rien : une mise à jour signée avec elle ne vaut rien) ;
3. regénérer et signer l'index avec la nouvelle clé ;
4. vérifier les paquets publiés depuis la compromission.
