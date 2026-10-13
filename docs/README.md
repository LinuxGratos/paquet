# Documentation

| Document | Pour qui | Contenu |
|---|---|---|
| [Utilisation de paquet](utilisation.md) | administrateurs d'un système Great-OS | configurer le dépôt, installer, mettre à jour, supprimer, réparer |
| [Construire des paquets avec pqmake](pqmake.md) | empaqueteurs | écrire une recette (`Pkgfile`), construire, construire en chroot |
| [Générer et publier un dépôt](depot.md) | mainteneurs d'un miroir | organisation du miroir, index `PQREPO`, publication, branches |
| [Signer un dépôt](signature.md) | mainteneurs d'un miroir | clés, signature de l'index, installation des clés, rotation |

## Le parcours complet

De la recette au système installé, en dix commandes. Les chemins sont des
exemples.

Côté construction (en utilisateur ordinaire) :

```sh
# 1. Une clé de signature, une fois pour toutes (à garder secrète)
genpqrepo --genkey ~/cles/great-os

# 2. Voir ce qui va être construit, puis construire
pqmake -R ~/recettes -o /srv/great-os/stable/x86_64 -n nano
pqmake -R ~/recettes -o /srv/great-os/stable/x86_64 nano

# 3. Générer l'index du miroir et le signer
genpqrepo -k ~/cles/great-os.key /srv/great-os/stable/x86_64
```

Les étapes 2 et 3 peuvent se faire en une fois :
`PQ_SIGN_KEY=~/cles/great-os.key pqmake --index nano`.

Côté système (en root), une fois :

```sh
# 4. Indiquer le miroir et faire confiance à la clé
echo 'MIRROR="https://miroir.exemple.org/great-os"' > /etc/pq.conf
install -Dm644 great-os.pub /etc/pq/keys/great-os.pub
```

Puis, au quotidien :

```sh
paquet --sync        # 5. récupère l'index et vérifie sa signature
paquet -s nano       # 6. cherche
paquet -i nano       # 7. installe, avec les dépendances
paquet -u            # 8. met à jour
paquet -r nano       # 9. supprime
paquet autoremove    # 10. supprime les dépendances devenues inutiles
```
