# `public-api/` — baseline de la surface publique du workspace

Ce répertoire contient la **surface publique commitée** de chaque crate publiable du
workspace, une ligne par item, un fichier par crate. Le gate CI `public-api`
re-mesure et diffe contre ces fichiers.

## Utilisation

```sh
./public-api/regen.sh --check    # ce que fait la CI : échoue si la surface a bougé
./public-api/regen.sh --write    # re-baseline, à commiter avec le changement d'API
```

Changer l'API publique **n'est pas interdit**. Ce qui est interdit, c'est de la
changer sans que le diff apparaisse quelque part : la re-baseline doit être dans le
même commit, où elle devient lisible en revue et rattachable au CHANGELOG.

## Périmètre — ce que le gate couvre

Les crates du workspace qui sont **publiables** *et* portent une cible `lib`. La
liste est calculée par `cargo metadata`, jamais par `grep publish` : un membre sans
clé `publish` explicite est publiable par défaut.

Mesuré au 2026-07-30 (`9b7bc5e6`) : **31 membres, 27 publiables, 26 avec cible lib**
(`gradatum-mcp-stub` était alors un binaire pur — publié, mais sans surface d'API), pour
**4 842 items**. Le décompte par crate vit dans `baseline/_INDEX.tsv`, régénéré et
vérifié comme le reste : il ne peut pas devenir périmé sans que le gate échoue.

> **Depuis `2.0.0` (2026-08-10)** : `gradatum-mcp-stub` bascule `publish = false` (retiré de la
> distribution, source conservée — voir `ARCHITECTURE.md` § API surface topology). Le décompte
> publiables ci-dessus passe donc à **26** ; le compte « avec cible lib » (26) est inchangé — le
> crate n'a jamais eu de cible `lib`, donc jamais de baseline dans ce répertoire (confirmé :
> aucun fichier `gradatum-mcp-stub` sous `baseline/`). Le total de 4 842 items n'a pas été
> re-mesuré dans cette passe.

Options de mesure :

- `--omit blanket-impls --omit auto-trait-impls --omit auto-derived-impls` — sans
  elles la sortie est majoritairement du bruit tiers (`typenum::Same`,
  `zerocopy::pointer::invariant::*`), et un gate lu en diagonale n'est pas lu.
- `--all-features` — la surface qu'un consommateur peut atteindre inclut les items
  derrière des features opt-in. Mesuré : la mesure par features par défaut manque
  **198 items**, dont **138 pour `gradatum-engine`** (feature `serve`), soit 98,6 %
  de cette crate, et 31 pour `gradatum-embed` (`fastembed-cpu`).

## Périmètre — ce que le gate NE couvre PAS

**Les items `#[doc(hidden)]`.** `cargo public-api` lit le rustdoc JSON, qui ne les
émet pas. Or `doc(hidden)` **masque la documentation, il ne retire rien de la surface
d'API** : un consommateur peut toujours les appeler, et un changement dessus reste une
rupture SemVer. Quatre crates publiées en dépendent et sont donc mesurées à 1 item :

| Crate | Items mesurés | Réalité |
|---|---|---|
| `gradatum-gateway` | 1 | 630 items masqués par ALIGN-SURFACE |
| `gradatum-studio` | 1 | modules `pub` masqués |
| `gradatum-worker` | 1 | modules `pub` masqués |
| `gradatum-admin` | 1 | modules `pub` masqués |

C'est un **choix**, et son coût est celui-ci : pour ces quatre crates, le gate est
vert par construction et le restera quoi qu'on y change. La contre-mesure n'est pas
dans cet outil — elle est de ne pas considérer leur surface comme couverte, et à
terme de rendre ces modules `pub(crate)` plutôt que `pub + doc(hidden)`, ce qui les
sortirait réellement de la surface publiée au lieu de les cacher.

**Les `impl From` générés par derive.** `--omit auto-derived-impls` ne masque pas que
`Clone`/`Debug`/`Eq` : il masque aussi les impls produites par `#[derive(Error)]` +
`#[from]` de `thiserror`. Mesuré sur le cas réel de I-013 — le retrait de
`impl From<serde_yml::Error> for MarkdownError` **n'apparaît pas** dans la baseline
avec les 3 `--omit`, alors qu'il apparaît sans le troisième :

```
cargo public-api -p gradatum-markdown --omit blanket-impls --omit auto-trait-impls diff 0.7.6
  -impl core::convert::From<serde_yml::modules::error::Error> for gradatum_markdown::MarkdownError
```

C'est un vrai breaking (tout `?` sur une erreur du backend cesse de compiler) et le
gate ne le voit pas. Le débruitage a donc un coût chiffré, et non nul : il achète une
sortie lisible contre la perte des conversions dérivées. Un changement qui ne touche
qu'un `#[from]` doit être vérifié à la main.

Ne sont pas couverts non plus : les ruptures qui ne changent pas la signature d'un
item (changement de comportement, de contrat, de valeur par défaut), et les crates
non publiables du workspace.

## Ce que ce gate ne remplace pas

`cargo semver-checks` ne le remplace pas et ne le double pas : mesuré au
2026-07-30 sur ce même cas, `check-release -p gradatum-markdown --release-type minor`
rend **196 checks, 196 pass, exit 0, « no semver update required »** alors que la
baseline `0.7.6` qu'il compare porte bien `Yaml(#[from] serde_yml::Error)`. Le gate a
tourné (build effectif, 196 lints non vides) et n'a rien vu. Les deux outils ont des
angles morts différents ; aucun des deux n'est une couverture.
